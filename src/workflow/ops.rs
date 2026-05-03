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
    /// Generic HTTP request → JSON-decoded response body. Workflow
    /// authors compose URL/headers/body from `${env.X}` + `${vars.X}`
    /// to express any code-host integration (issue fetch, PR create,
    /// PR comment, …) without baking platform-specific ops into the
    /// engine. Captures the response into `vars[into_var]` when set.
    HttpJson,
    /// Find the first element of an array variable whose nested field
    /// equals a target value, write into `vars[into_var]`. Writes
    /// `Value::Null` (not Err) when no match is found, so downstream
    /// `IsNull` / `IsNonNull` packet predicates can branch cleanly.
    /// Composable primitive that lets workflow authors express
    /// "find existing PR for this branch" / "find label by name" / etc
    /// without a code-host-specific search op AND without relying on
    /// upstream API filters that may be broken or absent.
    FindFirst,
    /// Outbound MCP tool call (sibling of HttpJson but speaking MCP
    /// JSON-RPC instead of REST). Resolves `args.server` against the
    /// blackbox MCP registry (global `~/.bro/mcp.json` + project
    /// overlay), opens a transient client (stdio child-process or
    /// streamable HTTP), invokes `args.tool` with `args.arguments`,
    /// and captures the result into `vars[into_var]`. Tool-level
    /// errors (`is_error: true`) become op failures so `on_failure`
    /// fires.
    ///
    /// Why an op kind, not just a dispatched bro: this lets the engine
    /// inject deterministic MCP results (`sast_run`, `sast_findings`,
    /// `bbox_thread`, etc) into hooks at known boundaries — analyzer
    /// arc creates a work-item thread before dispatch, fixer arc
    /// re-runs SAST on its branch before opening a PR, reviewer arc
    /// reads finding context the analyzer captured. Without this, the
    /// arc has to dispatch a bro to make every grounding call, which
    /// is both expensive and non-deterministic.
    McpCall,
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
#[derive(Debug)]
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
        OpKind::WorktreeRemove => exec_worktree_remove(&rendered_args, ctx).await,
        OpKind::SetMeta => exec_set_meta(&rendered_args),
        OpKind::HttpJson => exec_http_json(&rendered_args, hook.into_var.as_deref()).await,
        OpKind::FindFirst => exec_find_first(&rendered_args, hook.into_var.as_deref()),
        OpKind::McpCall => exec_mcp_call(&rendered_args, hook.into_var.as_deref(), ctx).await,
    }
}

async fn exec_mcp_call(args: &Value, into_var: Option<&str>, ctx: &ArcContext) -> Result<OpEffect> {
    let server = args.get("server").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow!("McpCall requires args.server (MCP registry name, e.g. 'biofilter' or 'blackbox')")
    })?;
    let tool = args.get("tool").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow!("McpCall requires args.tool (tool name as the server registers it)")
    })?;
    // arguments — must be an object (or absent → empty).
    let arguments = match args.get("arguments") {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::Null) | None => serde_json::Map::new(),
        Some(other) => bail!("McpCall args.arguments must be an object, got {other:?}"),
    };
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(300);
    let project_dir = ctx.meta.project_dir.as_deref();
    // cwd resolution: explicit args.cwd → arc's active worktree →
    // arc's project_dir → None (daemon's cwd, almost never what you
    // want for tools that read project state). MCP servers like
    // biofilter resolve their root via `process.cwd()` so this
    // matters for stdio transports.
    let cwd_owned: Option<String> = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.worktree.clone())
        .or_else(|| ctx.meta.project_dir.clone());
    let result = crate::mcp_client::call_tool(
        server,
        tool,
        arguments,
        timeout_secs,
        project_dir,
        cwd_owned.as_deref(),
    )
    .await
    .map_err(|e| anyhow!("McpCall '{server}.{tool}': {e}"))?;
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value: result,
        }),
        None => Ok(OpEffect::None),
    }
}

fn exec_find_first(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var.ok_or_else(|| anyhow!("FindFirst requires into_var on the HookOp spec"))?;
    let arr = args
        .get("from")
        .ok_or_else(|| anyhow!("FindFirst requires args.from (array)"))?;
    let where_obj = args
        .get("where")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("FindFirst requires args.where (object of field→value)"))?;
    let items: &[Value] = match arr {
        Value::Array(a) => a.as_slice(),
        Value::Null => &[],
        other => bail!("FindFirst args.from must be array or null, got {other:?}"),
    };
    let mut found: Value = Value::Null;
    'outer: for item in items {
        for (k, expected) in where_obj {
            // Walk dotted path inside the element.
            let actual = walk_dotted(item, k);
            if actual.as_ref() != Some(expected) {
                continue 'outer;
            }
        }
        found = item.clone();
        break;
    }
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: found,
    })
}

fn walk_dotted(root: &Value, path: &str) -> Option<Value> {
    let mut cur = root.clone();
    for seg in path.split('.') {
        cur = match &cur {
            Value::Object(m) => m.get(seg).cloned()?,
            Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx).cloned()?
            }
            _ => return None,
        };
    }
    Some(cur)
}

async fn exec_http_json(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    // Parse + execute via the shared HTTP-fetch primitive — same shape
    // the daemon-level poller consumes. The op is a thin wrapper that
    // also handles `into_var` capture; everything else (request build,
    // status classification, response decoding) lives in http_fetch.
    let spec = crate::orchestration::http_fetch::HttpFetchSpec::from_args(args)?;
    let result = spec.execute().await?;
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value: result.value,
        }),
        None => Ok(OpEffect::None),
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
    let into = into_var.ok_or_else(|| anyhow!("ParseJson requires into_var on the HookOp spec"))?;
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
                serde_json::from_str(&stripped)
                    .map_err(|e| anyhow!("ParseJson: input did not parse as JSON: {e}"))?
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
    let opens_fence = first == "```json" || first == "```JSON" || first == "```";
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

    // Refuse to clobber an existing path. Different issue from the
    // existing-branch case below — a path collision means another arc
    // is concurrently using this exact path or a previous one wasn't
    // cleaned up. Either way, don't silently mutate.
    if Path::new(&path).exists() {
        bail!("WorktreeCreate: path {path} already exists");
    }

    // Branch existence check. Failed/killed arcs leave their branch
    // behind (intentional under `keep-on-fail` cleanup policy), so
    // creating a fresh branch with the same name (`-b`) on the next
    // attempt blows up with `fatal: a branch named '<x>' already
    // exists`. Two real cases:
    //   (a) branch exists AND is checked out in another worktree → real
    //       conflict (concurrent arc on same issue), fail loudly
    //   (b) branch exists but no worktree references it → reuse it
    //       (`git worktree add <path> <branch>` without `-b`)
    //   (c) branch doesn't exist → fresh create (`-b <branch>` against
    //       <base>)
    let branch_status = branch_state(&repo_root, &branch).await?;
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(&repo_root).arg("worktree").arg("add");
    match branch_status {
        BranchState::AbsentLocal => {
            // Case (c): create fresh.
            cmd.arg("-b").arg(&branch).arg(&path).arg(&base);
        }
        BranchState::PresentFree => {
            // Case (b): reuse existing branch — no `-b`.
            cmd.arg(&path).arg(&branch);
        }
        BranchState::PresentInUse(other_path) => {
            // Case (a): real conflict.
            bail!(
                "WorktreeCreate: branch '{branch}' is already checked out at {other_path} \
                 (concurrent arc on same issue?). Either let the other arc finish, or \
                 retire its worktree, or use a unique branch name (e.g. include \
                 ${{meta.arc_id}} in the name)."
            );
        }
    }
    let output = cmd
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

/// State of a local branch with respect to worktree usage.
enum BranchState {
    /// Branch does not exist locally.
    AbsentLocal,
    /// Branch exists but is not checked out by any worktree.
    PresentFree,
    /// Branch exists AND another worktree has it checked out (path).
    PresentInUse(String),
}

/// Inspect a branch's worktree-occupancy in `repo_root`.
async fn branch_state(repo_root: &str, branch: &str) -> Result<BranchState> {
    // First: does the branch ref exist at all?
    let exists = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show-ref")
        .arg("--verify")
        .arg("--quiet")
        .arg(format!("refs/heads/{branch}"))
        .status()
        .await
        .map_err(|e| anyhow!("git show-ref spawn: {e}"))?
        .success();
    if !exists {
        return Ok(BranchState::AbsentLocal);
    }
    // Second: is some worktree currently checked out on it?
    let porcelain = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()
        .await
        .map_err(|e| anyhow!("git worktree list spawn: {e}"))?;
    if !porcelain.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&porcelain.stderr)
        );
    }
    let text = String::from_utf8_lossy(&porcelain.stdout).into_owned();
    // Records are separated by blank lines; each starts with `worktree
    // <path>` and may include `branch refs/heads/<name>`.
    let needle = format!("branch refs/heads/{branch}");
    let mut current_path: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(p.to_string());
            continue;
        }
        if line.trim() == needle {
            if let Some(p) = current_path.take() {
                return Ok(BranchState::PresentInUse(p));
            }
        }
        if line.trim().is_empty() {
            current_path = None;
        }
    }
    Ok(BranchState::PresentFree)
}

async fn exec_worktree_remove(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
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

    // `git worktree remove` is a porcelain that needs to run inside
    // the *parent* repo (the one that owns the worktree), not inside
    // the worktree itself. Default to meta.project_dir; allow the
    // workflow to override via args.repo_root if it knows better.
    let repo_root = args
        .get("repo_root")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.project_dir.clone())
        .ok_or_else(|| {
            anyhow!(
                "WorktreeRemove: no repo_root resolvable (set args.repo_root or meta.project_dir)"
            )
        })?;

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("worktree")
        .arg("remove")
        .arg(&path);
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
        // No `rm -rf` fallback — that would erase whatever path the
        // workflow author templated, with no containment guarantee.
        // Operator can clean a non-git-tracked path manually; engine
        // surfaces the git error verbatim so the cause is visible.
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

        let mut meta = ArcMeta::default();
        meta.project_dir = Some(repo.to_string_lossy().into_owned());
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

        let mut meta = ArcMeta::default();
        meta.project_dir = Some(repo.to_string_lossy().into_owned());
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
}
