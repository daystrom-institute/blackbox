//! Bounded read-only Git primitives. Subprocess argv is never shell-parsed.

use super::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;

const READ_TIME: Duration = Duration::from_secs(30);
const LIST_BYTES: usize = 1024 * 1024;

pub(crate) async fn capture_git(
    cx: &ToolCx,
    root: &Path,
    args: &[OsString],
    until: Instant,
    bytes: usize,
) -> Result<crate::shell::CommandCapture, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.quotePath=true",
        ])
        .args(args)
        .current_dir(root);
    cx.child_env.apply(command.as_std_mut());
    command.envs(cx.shell_env.iter());
    // Never allow an interactive credential prompt in a finite tool invocation.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    #[cfg(test)]
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    crate::shell::run_supervised_command(
        command,
        cx,
        until.saturating_duration_since(Instant::now()),
        bytes,
        bytes.min(64 * 1024),
    )
    .await
}

fn argv(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}
fn budget(cx: &ToolCx) -> usize {
    if cx.output_budget == 0 {
        8000
    } else {
        cx.output_budget.min(8000)
    }
}

fn render(capture: crate::shell::CommandCapture, cap: usize, note: &str) -> ToolResult {
    let mut body = String::from_utf8_lossy(&capture.stdout).into_owned();
    if !capture.stderr.is_empty() {
        body.push_str("\n[stderr]\n");
        body.push_str(&String::from_utf8_lossy(&capture.stderr));
    }
    let invalid = std::str::from_utf8(&capture.stdout).is_err()
        || std::str::from_utf8(&capture.stderr).is_err();
    let failed = capture.exit_code != Some(0)
        || capture.timed_out
        || capture.cancelled
        || !capture.errors.is_empty();
    let mut disclosure = if !capture.complete() || invalid || failed {
        format!(
            "\n[git capture: {}; invalid_utf8_replaced={invalid}]",
            capture.facts()
        )
    } else {
        String::new()
    };
    disclosure.push_str(note);
    let body_cap = cap.saturating_sub(disclosure.len());
    let body = crate::output::truncate_text(&body, body_cap);
    let output = crate::output::truncate_text(&format!("{body}{disclosure}"), cap);
    if failed {
        ToolResult::Error(output)
    } else {
        ToolResult::Text(output)
    }
}

async fn git(cx: &ToolCx, args: &[&str]) -> ToolResult {
    let root = super::effective_root(&cx.root);
    match capture_git(
        cx,
        &root,
        &argv(args),
        Instant::now() + READ_TIME,
        budget(cx),
    )
    .await
    {
        Ok(capture) => render(capture, budget(cx), ""),
        Err(error) => ToolResult::Error(error),
    }
}

macro_rules! read_git_tool {
    ($ty:ident, $name:literal, $desc:literal, $argv:expr) => {
        pub struct $ty;
        #[async_trait]
        impl Tool for $ty {
            fn name(&self) -> &str { $name }
            fn description(&self) -> &str { $desc }
            fn input_schema(&self) -> Value { json!({"type":"object", "properties":{}}) }
            fn annotations(&self) -> ToolAnnotations { ToolAnnotations { read_only:true, ..Default::default() } }
            async fn call(&self, _: Value, cx: &ToolCx) -> ToolResult { git(cx, $argv).await }
        }
    };
}
read_git_tool!(
    GitStatus,
    "git_status",
    "Show git status --short with bounded capture and a 30-second deadline; omissions are disclosed.",
    &["status", "--short"]
);
read_git_tool!(
    GitLog,
    "git_log",
    "Show the latest 20 commits with bounded capture and a 30-second deadline; omissions are disclosed.",
    &["log", "--oneline", "-20"]
);

#[derive(Deserialize, JsonSchema)]
struct GitDiffInput {
    /// Include bounded untracked files as new-file patches.
    include_untracked: Option<bool>,
    /// Explicit literal file or directory paths to scope both tracked and untracked diffs.
    paths: Option<Vec<String>>,
    /// Maximum untracked files to inspect (default 20, maximum 100). Must be positive.
    max_untracked_files: Option<usize>,
}
pub struct GitDiff;
#[async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &str {
        "git_diff"
    }
    fn description(&self) -> &str {
        "Show unstaged changes in a bounded diff (at most 8000 output bytes, 30 seconds total). Optional paths are literal files/directories. include_untracked=true inspects at most max_untracked_files (default 20, max 100), stopping when the output budget fills; omitted files/output are disclosed. Narrow paths for more detail."
    }
    fn input_schema(&self) -> Value {
        schema_for::<GitDiffInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: GitDiffInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(error) => return ToolResult::Error(error.to_string()),
        };
        if args.max_untracked_files == Some(0) || args.paths.as_ref().is_some_and(Vec::is_empty) {
            return ToolResult::Error(
                "max_untracked_files must be positive and paths must not be an empty list".into(),
            );
        }
        let root = super::effective_root(&cx.root);
        let until = Instant::now() + READ_TIME;
        let cap = budget(cx);
        let mut tracked_args = argv(&["diff", "--no-ext-diff", "--"]);
        if let Some(paths) = &args.paths {
            tracked_args.extend(paths.iter().map(OsString::from));
        }
        let mut capture = match capture_git(cx, &root, &tracked_args, until, cap).await {
            Ok(result) => result,
            Err(error) => return ToolResult::Error(error),
        };
        if !args.include_untracked.unwrap_or(false)
            || capture.exit_code != Some(0)
            || !capture.complete()
        {
            return render(capture, cap, "");
        }
        let mut list_args = argv(&["ls-files", "--others", "--exclude-standard", "-z", "--"]);
        if let Some(paths) = &args.paths {
            list_args.extend(paths.iter().map(OsString::from));
        }
        let listing = match capture_git(cx, &root, &list_args, until, LIST_BYTES).await {
            Ok(result) => result,
            Err(error) => return ToolResult::Error(error),
        };
        if listing.exit_code != Some(0) || !listing.complete() {
            return ToolResult::Error(format!(
                "Untracked file enumeration incomplete; narrow paths. {}",
                listing.facts()
            ));
        }
        let paths: Vec<_> = listing
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .collect();
        let max_files = args.max_untracked_files.unwrap_or(20).min(100);
        let mut inspected = 0;
        for path in paths.iter().take(max_files) {
            if capture.stdout.len() >= cap
                || cx.cancellation.is_cancelled()
                || Instant::now() >= until
            {
                break;
            }
            #[cfg(unix)]
            let literal = {
                use std::os::unix::ffi::OsStringExt;
                OsString::from_vec(path.to_vec())
            };
            #[cfg(not(unix))]
            let literal = match std::str::from_utf8(path) {
                Ok(path) => OsString::from(path),
                Err(_) => {
                    return ToolResult::Error(
                        "untracked filename is not representable as UTF-8 on this platform".into(),
                    );
                }
            };
            let mut patch_args = argv(&["diff", "--no-index", "--no-ext-diff", "--", "/dev/null"]);
            patch_args.push(literal);
            let patch = match capture_git(
                cx,
                &root,
                &patch_args,
                until,
                cap.saturating_sub(capture.stdout.len()),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => return ToolResult::Error(error),
            };
            inspected += 1;
            capture.stdout.extend_from_slice(&patch.stdout);
            capture.stdout_dropped += patch.stdout_dropped;
            let room = cap.saturating_sub(capture.stderr.len());
            let keep = room.min(patch.stderr.len());
            capture.stderr.extend_from_slice(&patch.stderr[..keep]);
            capture.stderr_dropped += patch.stderr_dropped + (patch.stderr.len() - keep) as u64;
            capture.timed_out |= patch.timed_out;
            capture.cancelled |= patch.cancelled;
            capture.errors.extend(patch.errors);
            if !matches!(patch.exit_code, Some(0 | 1)) {
                capture.exit_code = patch.exit_code;
                break;
            }
            if capture.timed_out || capture.cancelled || capture.stdout_dropped > 0 {
                break;
            }
        }
        let omitted = paths.len() - inspected;
        let note = if omitted > 0 {
            format!(
                "\n[untracked files inspected={inspected}; omitted={omitted}; narrow paths or increase max_untracked_files within the output limit]\n"
            )
        } else {
            String::new()
        };
        if cx.cancellation.is_cancelled() {
            capture.cancelled = true;
        }
        if Instant::now() >= until {
            capture.timed_out = true;
        }
        render(capture, cap, &note)
    }
}

#[derive(Deserialize, JsonSchema)]
struct GitShowInput {
    /// Commit-ish to show (default HEAD).
    rev: Option<String>,
}
pub struct GitShow;
#[async_trait]
impl Tool for GitShow {
    fn name(&self) -> &str {
        "git_show"
    }
    fn description(&self) -> &str {
        "Show a commit with bounded capture and a 30-second deadline; output omissions are disclosed."
    }
    fn input_schema(&self) -> Value {
        schema_for::<GitShowInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: GitShowInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(error) => return ToolResult::Error(format!("bad input: {error}")),
        };
        git(
            cx,
            &[
                "show",
                "--no-ext-diff",
                "--end-of-options",
                args.rev.as_deref().unwrap_or("HEAD"),
            ],
        )
        .await
    }
}

pub(crate) async fn status_manifest(cx: &ToolCx, root: &Path, status_limit: usize) -> Value {
    let until = Instant::now() + READ_TIME;
    let mut manifest = serde_json::Map::new();
    let mut incomplete = Vec::new();
    for (name, args) in [
        ("toplevel", vec!["rev-parse", "--show-toplevel"]),
        ("branch", vec!["rev-parse", "--abbrev-ref", "HEAD"]),
        ("head", vec!["rev-parse", "--short=12", "HEAD"]),
    ] {
        match capture_git(cx, root, &argv(&args), until, 4096).await {
            Ok(capture) if capture.exit_code == Some(0) && capture.complete() => {
                match String::from_utf8(capture.stdout) {
                    Ok(text) => {
                        manifest.insert(name.into(), json!(text.trim()));
                    }
                    Err(_) => {
                        manifest.insert(name.into(), Value::Null);
                        incomplete.push(json!({"command":name,"error":"non-UTF-8 output"}));
                    }
                }
            }
            Ok(capture) => {
                manifest.insert(name.into(), Value::Null);
                incomplete.push(json!({"command":name,"capture":capture.facts()}));
            }
            Err(error) => {
                manifest.insert(name.into(), Value::Null);
                incomplete.push(json!({"command":name,"error":error}));
            }
        }
    }
    match capture_git(
        cx,
        root,
        &argv(&["status", "--short", "--branch"]),
        until,
        LIST_BYTES,
    )
    .await
    {
        Ok(capture) if capture.exit_code == Some(0) => {
            let text = String::from_utf8_lossy(&capture.stdout);
            let mut lines = text.lines();
            let branch = lines.next().unwrap_or_default();
            let mut entries: Vec<_> = lines.collect();
            if !capture.stdout.ends_with(b"\n") && capture.stdout_dropped > 0 {
                entries.pop();
            }
            let count = entries.len();
            let complete = capture.complete() && std::str::from_utf8(&capture.stdout).is_ok();
            manifest.insert(
                "status".into(),
                json!({
                    "branch_line":branch, "dirty_count":complete.then_some(count),
                    "dirty_entries_observed":count,
                    "entries":entries.into_iter().take(status_limit.min(200)).collect::<Vec<_>>(),
                    "truncated": !complete || count > status_limit.min(200),
                }),
            );
            if !complete {
                incomplete.push(json!({"command":"status", "capture":capture.facts()}));
            }
        }
        Ok(capture) => {
            manifest.insert("status".into(), Value::Null);
            incomplete.push(json!({"command":"status","capture":capture.facts()}));
        }
        Err(error) => {
            manifest.insert("status".into(), Value::Null);
            incomplete.push(json!({"command":"status","error":error}));
        }
    }
    manifest.insert("incomplete_commands".into(), json!(incomplete));
    Value::Object(manifest)
}
