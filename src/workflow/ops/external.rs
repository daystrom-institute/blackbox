use std::process::Stdio;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;

use crate::workflow::context::ArcContext;

use super::OpEffect;

pub(super) async fn exec_mcp_call(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let server = args.get("server").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow!("McpCall requires args.server (MCP registry name, e.g. 'biofilter' or 'blackbox')")
    })?;
    let tool = args.get("tool").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow!("McpCall requires args.tool (tool name as the server registers it)")
    })?;
    let arguments = match args.get("arguments") {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::Null) | None => Map::new(),
        Some(other) => bail!("McpCall args.arguments must be an object, got {other:?}"),
    };
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(300);
    let project_dir = ctx.meta.project_dir.as_deref();
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

pub(super) async fn exec_http_json(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    // Parse + execute via the shared HTTP-fetch primitive — same shape
    // the daemon-level poller consumes. The op is a thin wrapper that
    // also handles `into_var` capture; everything else (request build,
    // status classification, response decoding) lives in http_fetch.
    let mut spec = crate::orchestration::http_fetch::HttpFetchSpec::from_args(args)?;
    // `secret_headers` resolves `secret:<name>` refs at request time so
    // raw token values never appear in vars, logs, or JSON artifacts.
    // Values are merged into spec.headers AFTER from_args so they override
    // any same-named key in the normal `headers` field.
    if let Some(secret_hdrs) = args.get("secret_headers").and_then(|v| v.as_object()) {
        for (header_name, raw_val) in secret_hdrs {
            let raw_str = raw_val
                .as_str()
                .ok_or_else(|| anyhow!("secret_headers.{header_name} must be a string"))?;
            let resolved = resolve_secret_header_value(raw_str)
                .map_err(|e| anyhow!("secret_headers.{header_name}: {e}"))?;
            spec.headers.insert(header_name.clone(), resolved);
        }
    }
    let result = spec.execute().await?;
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value: result.value,
        }),
        None => Ok(OpEffect::None),
    }
}

/// Match one `argv[0]` value against an allowlist entry set. Byte-exact
/// equality only, with the match SHAPE constrained by whether `argv0`
/// carries a path separator:
///
/// - A bare `argv0` (no `/`) matches only a bare allowlist entry equal to
///   it verbatim, e.g. entry `"git"` matches `argv0 == "git"`.
/// - A path-bearing `argv0` (`/usr/bin/git`, `./git`, `bin/git`, …)
///   matches only an allowlist entry that is the exact same string. A
///   bare entry like `"git"` does NOT authorize any path-bearing argv0,
///   even `/usr/bin/git`: that basename-matching shortcut let an
///   allowlisted bare name stand in for an arbitrary path (`/evil/git`
///   also has basename `git`), which defeats the allowlist as a control.
///
/// No globs, no prefix matching, no directory-relative resolution.
pub(super) fn shell_argv0_allowed(argv0: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|entry| entry == argv0)
}

/// Parse a Shell op's own `args.allowlist` (a per-node override that can
/// only NARROW the workflow-level `shell_allowlist`, never widen it — see
/// `exec_shell`). Absent key = `None` (no per-op narrowing; the
/// workflow-level list, if any, still applies on its own). Present but
/// malformed (not an array, or containing a non-string element) is a
/// hard error: a security allowlist that fails to parse must fail
/// CLOSED, never silently collapse to "no restriction."
pub(super) fn parse_shell_allowlist_arg(args: &Value) -> Result<Option<Vec<String>>> {
    match args.get("allowlist") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let list = items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("Shell args.allowlist entries must be strings"))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Some(list))
        }
        Some(_) => bail!("Shell args.allowlist must be an array of strings"),
    }
}

/// Execute a shell command. When `into_var` is provided the op captures
/// `{exit_code, stdout, stderr, parsed}` into `vars[into_var]` and does NOT
/// fail on a non-zero exit code — the caller decides what to do with the
/// exit code via downstream ops or packet gates. Without `into_var` the
/// original behaviour is preserved: non-zero exit aborts the op.
///
/// `allowlist`, when `Some`, restricts `argv[0]` to entries matched by
/// [`shell_argv0_allowed`]; a non-matching command fails closed before
/// anything spawns. `None` preserves the pre-allowlist trusted-actor
/// behavior of running any argv.
pub(super) async fn exec_shell(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
    allowlist: Option<&[String]>,
) -> Result<OpEffect> {
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
    if let Some(list) = allowlist
        && !shell_argv0_allowed(&strs[0], list)
    {
        bail!(
            "Shell argv[0] '{}' is not on the workflow shell allowlist {list:?}",
            strs[0]
        );
    }
    // Workflow-level allowlist rides ArcMeta (copied from the compiled
    // spec at runner construction) and is enforced in ADDITION to the
    // per-op list above: a per-op `args.allowlist` can narrow the
    // workflow policy but never widen it.
    if let Some(list) = ctx.meta.shell_allowlist.as_deref()
        && !shell_argv0_allowed(&strs[0], list)
    {
        bail!(
            "Shell argv[0] '{}' is not on the workflow-level shell allowlist {list:?}",
            strs[0]
        );
    }
    // With a shell policy active, an env PATH override would defeat
    // bare-name authorization: tokio's Command resolves a bare argv[0]
    // through its CONFIGURED environment, so permitting
    // `env: {"PATH": "/tmp/attacker-bin"}` lets an allowlisted "git"
    // execute /tmp/attacker-bin/git. Fail closed on either env section.
    if allowlist.is_some() || ctx.meta.shell_allowlist.is_some() {
        for (section, value) in [
            ("env", args.get("env")),
            ("secret_env", args.get("secret_env")),
        ] {
            if let Some(obj) = value.and_then(|v| v.as_object())
                && obj.contains_key("PATH")
            {
                bail!(
                    "Shell {section}.PATH override is not allowed while a shell allowlist is active"
                );
            }
        }
    }
    let cwd = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.worktree.clone())
        .or_else(|| ctx.meta.project_dir.clone());
    let stdin_payload = args
        .get("stdin")
        .map(shell_stdin_payload)
        .transpose()
        .map_err(|e| anyhow!("Shell stdin: {e}"))?;
    let mut cmd = tokio::process::Command::new(&strs[0]);
    cmd.args(&strs[1..])
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = &cwd {
        cmd.current_dir(d);
    }
    if let Some(env) = args.get("env").and_then(|v| v.as_object()) {
        for (key, value) in env {
            let value = value
                .as_str()
                .ok_or_else(|| anyhow!("Shell env.{key} must be a string"))?;
            cmd.env(key, value);
        }
    }
    if let Some(secret_env) = args.get("secret_env").and_then(|v| v.as_object()) {
        for (key, raw_value) in secret_env {
            let raw_value = raw_value
                .as_str()
                .ok_or_else(|| anyhow!("Shell secret_env.{key} must be a string"))?;
            let resolved = resolve_secret_header_value(raw_value)
                .map_err(|e| anyhow!("Shell secret_env.{key}: {e}"))?;
            cmd.env(key, resolved);
        }
    }
    let output = if let Some(payload) = stdin_payload {
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Shell spawn failed: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(payload.as_bytes())
                .await
                .map_err(|e| anyhow!("Shell stdin write failed: {e}"))?;
        }
        child
            .wait_with_output()
            .await
            .map_err(|e| anyhow!("Shell wait failed: {e}"))?
    } else {
        cmd.output()
            .await
            .map_err(|e| anyhow!("Shell spawn failed: {e}"))?
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();

    if let Some(key) = into_var {
        let parsed: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        return Ok(OpEffect::SetVar {
            key: key.to_string(),
            value: json!({
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr_str,
                "parsed": parsed,
            }),
        });
    }

    if !output.status.success() {
        bail!("Shell {strs:?} exited with {exit_code}: {stderr_str}",);
    }
    Ok(OpEffect::None)
}

fn shell_stdin_payload(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Ok(serde_json::to_string(other)?),
    }
}

/// Resolve a header value that contains a `secret:<name>` reference.
/// The `secret:<name>` substring is replaced with the resolved secret value.
/// Example: `"token secret:my-token"` → `"token <actual-value>"`.
/// Rejects values that contain no `secret:` reference — use the normal
/// `headers` field for static values.
fn resolve_secret_header_value(value: &str) -> Result<String> {
    let Some(start) = value.find("secret:") else {
        bail!(
            "secret_headers value must contain a 'secret:<name>' reference; \
             use the 'headers' field for static values"
        )
    };
    let rest = &value[start + "secret:".len()..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let name = &rest[..end];
    // resolve() validates the name and returns an error without exposing the value.
    let secret = blackbox::secrets::resolve(name)
        .map_err(|_| anyhow!("secret_headers: secret '{name}' could not be resolved"))?;
    let prefix = &value[..start];
    let suffix = &rest[end..];
    Ok(format!("{}{}{}", prefix, secret.expose(), suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_argv0_allowed_exact_bare_match() {
        let allowlist = vec!["git".to_string(), "cargo".to_string()];
        assert!(shell_argv0_allowed("git", &allowlist));
        assert!(shell_argv0_allowed("cargo", &allowlist));
    }

    #[test]
    fn shell_argv0_allowed_bare_entry_rejects_path_bearing_argv0() {
        // A bare allowlist entry ("git") must NOT authorize a
        // path-bearing argv0 sharing its basename — that was the
        // /evil/git-shares-basename-with-git loophole.
        let allowlist = vec!["git".to_string()];
        assert!(!shell_argv0_allowed("/usr/bin/git", &allowlist));
        assert!(!shell_argv0_allowed("./git", &allowlist));
        assert!(!shell_argv0_allowed("/evil/git", &allowlist));
    }

    #[test]
    fn shell_argv0_allowed_absolute_path_exact_match() {
        let allowlist = vec!["/usr/bin/git".to_string()];
        assert!(shell_argv0_allowed("/usr/bin/git", &allowlist));
        // A different absolute path with the same basename does not match
        // an allowlist entry that is itself an absolute path.
        assert!(!shell_argv0_allowed("/opt/bin/git", &allowlist));
        // Nor does the bare basename authorize via a path-bearing entry.
        assert!(!shell_argv0_allowed("git", &allowlist));
    }

    #[test]
    fn shell_argv0_allowed_relative_path_exact_match() {
        let allowlist = vec!["./git".to_string()];
        assert!(shell_argv0_allowed("./git", &allowlist));
        assert!(!shell_argv0_allowed("git", &allowlist));
        assert!(!shell_argv0_allowed("/usr/bin/git", &allowlist));
        assert!(!shell_argv0_allowed("bin/git", &allowlist));
    }

    #[test]
    fn shell_argv0_allowed_rejects_unlisted() {
        let allowlist = vec!["git".to_string(), "cargo".to_string()];
        assert!(!shell_argv0_allowed("rm", &allowlist));
        assert!(!shell_argv0_allowed("/usr/bin/rm", &allowlist));
    }

    #[test]
    fn shell_argv0_allowed_no_glob_no_prefix() {
        let allowlist = vec!["git*".to_string(), "gi".to_string()];
        assert!(!shell_argv0_allowed("git", &allowlist));
        assert!(!shell_argv0_allowed("github", &allowlist));
    }

    #[test]
    fn shell_argv0_allowed_empty_list_denies_all() {
        assert!(!shell_argv0_allowed("true", &[]));
    }

    #[test]
    fn parse_shell_allowlist_arg_absent_is_none() {
        assert_eq!(
            parse_shell_allowlist_arg(&serde_json::json!({"argv": ["true"]})).unwrap(),
            None
        );
    }

    #[test]
    fn parse_shell_allowlist_arg_null_is_none() {
        assert_eq!(
            parse_shell_allowlist_arg(&serde_json::json!({"allowlist": null})).unwrap(),
            None
        );
    }

    #[test]
    fn parse_shell_allowlist_arg_parses_string_array() {
        let parsed = parse_shell_allowlist_arg(&serde_json::json!({
            "allowlist": ["git", "cargo"]
        }))
        .unwrap();
        assert_eq!(parsed, Some(vec!["git".to_string(), "cargo".to_string()]));
    }

    #[test]
    fn parse_shell_allowlist_arg_non_array_fails_closed() {
        let err = parse_shell_allowlist_arg(&serde_json::json!({"allowlist": "git"})).unwrap_err();
        assert!(
            err.to_string().contains("must be an array"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_shell_allowlist_arg_non_string_entry_fails_closed() {
        let err =
            parse_shell_allowlist_arg(&serde_json::json!({"allowlist": ["git", 7]})).unwrap_err();
        assert!(
            err.to_string().contains("must be strings"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_secret_header_value_with_env_fallback() {
        let _env = crate::util::test_env_lock();
        // Use the env var fallback (BLACKBOX_SECRET_TEST_HTTP_TOKEN).
        // SAFETY: single-threaded test context; no other threads access this var.
        unsafe { std::env::set_var("BLACKBOX_SECRET_TEST_HTTP_TOKEN", "tok-abc123") };
        let result = resolve_secret_header_value("token secret:test-http-token").unwrap();
        assert_eq!(result, "token tok-abc123");
        // SAFETY: same as above.
        unsafe { std::env::remove_var("BLACKBOX_SECRET_TEST_HTTP_TOKEN") };
    }

    #[test]
    fn resolve_secret_header_value_rejects_no_secret_ref() {
        let err = resolve_secret_header_value("static-plain-value").unwrap_err();
        assert!(
            err.to_string().contains("secret:"),
            "expected rejection, got: {err}"
        );
    }

    #[test]
    fn resolve_secret_header_value_missing_secret_errors_without_value() {
        let err = resolve_secret_header_value("Bearer secret:definitely-not-set-xyz").unwrap_err();
        let msg = err.to_string();
        // Must not contain any resolved value (there is none), but must name the key.
        assert!(
            msg.contains("definitely-not-set-xyz"),
            "expected secret name in error: {msg}"
        );
        assert!(
            !msg.contains("Bearer"),
            "error must not expose header prefix: {msg}"
        );
    }

    #[test]
    fn resolve_secret_header_value_bare_secret_ref() {
        let _env = crate::util::test_env_lock();
        // SAFETY: single-threaded test context; no other threads access this var.
        unsafe { std::env::set_var("BLACKBOX_SECRET_BARE_TOKEN", "raw-value") };
        let result = resolve_secret_header_value("secret:bare-token").unwrap();
        assert_eq!(result, "raw-value");
        // SAFETY: same as above.
        unsafe { std::env::remove_var("BLACKBOX_SECRET_BARE_TOKEN") };
    }
}
