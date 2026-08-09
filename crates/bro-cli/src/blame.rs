use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bbox_corpus_core::blame_transport::{BlameExecutionPlanV1, execute_plan_in_workspace};
use bro_rpc::ServiceToken;
use clap::Args;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::mcp_call::{McpClient, default_base_url};

const BLAME_TOOL: &str = "bbox_blame";

#[derive(Debug, Args)]
pub(crate) struct BlameArgs {
    /// Checkout project root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    project_root: Option<PathBuf>,
    /// Scope-bound producer bearer file. The token is sent only during MCP
    /// initialization and is never put in the URL or output.
    #[arg(long, value_name = "FILE")]
    token_file: PathBuf,
    /// Daemon base URL. Remote URLs require HTTPS; loopback HTTP is allowed.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
    /// File inside the project checkout. Requires --line.
    #[arg(long, value_name = "FILE", conflicts_with = "entity_ref")]
    file: Option<PathBuf>,
    /// Corpus project_file entity reference. Uses its indexed byte offset
    /// unless --line is supplied.
    #[arg(long, value_name = "REF", conflicts_with = "file")]
    entity_ref: Option<String>,
    /// One-based source line.
    #[arg(long)]
    line: Option<u64>,
}

#[derive(Deserialize)]
struct PlanResponse {
    status: String,
    plan: BlameExecutionPlanV1,
}

pub(crate) async fn run(args: BlameArgs) -> anyhow::Result<()> {
    let requested_root = match args.project_root {
        Some(root) => root,
        None => std::env::current_dir().context("reading current directory")?,
    };
    let project_root = requested_root.canonicalize().with_context(|| {
        format!(
            "canonicalizing blame project root {}",
            requested_root.display()
        )
    })?;
    if !project_root.is_dir() {
        bail!("blame project root is not a directory");
    }
    let workspace_root = bbox_corpus_core::git::git_root_for_path(&project_root)
        .context("blame project is not inside a Git checkout")?
        .canonicalize()
        .context("canonicalizing blame Git root")?;
    if !project_root.starts_with(&workspace_root) {
        bail!("blame project root is outside its Git checkout");
    }
    let scope = bbox_provenance::resolve_committed_scope(&project_root)
        .context("resolving committed blame project scope")?;
    let workspace_id = bbox_corpus_core::identity::ensure_checkout_id(&workspace_root)
        .context("ensuring blame checkout identity")?;
    let token = ServiceToken::load(&args.token_file)
        .with_context(|| format!("loading {}", args.token_file.display()))?;
    let public_arguments = public_arguments(
        &project_root,
        args.file.as_deref(),
        args.entity_ref.as_deref(),
        args.line,
    )?;

    let base_url = args.daemon_url.unwrap_or_else(default_base_url);
    let mut client =
        McpClient::connect_with_operator_blame(&base_url, &token, &scope, &workspace_id).await?;
    drop(token);

    let mut plan_arguments = public_arguments.clone();
    insert_locality(&mut plan_arguments, json!({"phase": "plan"}))?;
    let response: PlanResponse = client.call_tool_json(BLAME_TOOL, plan_arguments).await?;
    if response.status != "blame_locality_plan" {
        bail!("daemon returned an unexpected blame planning status");
    }
    let fact = execute_plan_in_workspace(
        &response.plan,
        &workspace_root,
        &project_root,
        &scope,
        &workspace_id,
    )?;
    let mut resolve_arguments = public_arguments;
    insert_locality(
        &mut resolve_arguments,
        json!({
            "phase": "resolve",
            "plan": response.plan,
            "fact": fact,
        }),
    )?;
    let result: Value = client.call_tool_json(BLAME_TOOL, resolve_arguments).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn public_arguments(
    project_root: &Path,
    file: Option<&Path>,
    entity_ref: Option<&str>,
    line: Option<u64>,
) -> anyhow::Result<Value> {
    if line.is_some_and(|line| line == 0) {
        bail!("blame line must be 1-based");
    }
    match (file, entity_ref) {
        (Some(file), None) => {
            let line = line.context("--line is required with --file")?;
            let requested = if file.is_absolute() {
                file.to_path_buf()
            } else {
                project_root.join(file)
            };
            let canonical = requested
                .canonicalize()
                .with_context(|| format!("canonicalizing blame file {}", requested.display()))?;
            if !canonical.is_file() || !canonical.starts_with(project_root) {
                bail!("blame file is outside the project root");
            }
            let relative = canonical
                .strip_prefix(project_root)
                .context("deriving project-relative blame file")?
                .to_string_lossy()
                .replace('\\', "/");
            Ok(json!({"file": relative, "line": line}))
        }
        (None, Some(entity_ref)) if !entity_ref.trim().is_empty() => {
            let mut arguments =
                Map::from_iter([("entity_ref".into(), Value::String(entity_ref.to_string()))]);
            if let Some(line) = line {
                arguments.insert("line".into(), Value::from(line));
            }
            Ok(Value::Object(arguments))
        }
        (None, Some(_)) => bail!("--entity-ref must not be empty"),
        (None, None) => bail!("provide exactly one of --file or --entity-ref"),
        (Some(_), Some(_)) => bail!("--file and --entity-ref are mutually exclusive"),
    }
}

fn insert_locality(arguments: &mut Value, locality: Value) -> anyhow::Result<()> {
    arguments
        .as_object_mut()
        .context("blame arguments are not an object")?
        .insert("_blame_locality".into(), locality);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_arguments_are_relative_and_confined() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        let arguments =
            public_arguments(&root, Some(Path::new("src/lib.rs")), None, Some(1)).unwrap();
        assert_eq!(arguments, json!({"file": "src/lib.rs", "line": 1}));

        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(
            public_arguments(&root, Some(outside.path()), None, Some(1))
                .unwrap_err()
                .to_string()
                .contains("outside the project root")
        );
    }

    #[test]
    fn entity_arguments_keep_line_optional() {
        assert_eq!(
            public_arguments(Path::new("."), None, Some("bbox:v1:project_file:x"), None).unwrap(),
            json!({"entity_ref": "bbox:v1:project_file:x"})
        );
        assert!(
            public_arguments(Path::new("."), None, None, None)
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[tokio::test]
    async fn operator_cli_executes_real_git_and_transports_only_the_fact() {
        use axum::Json;
        use axum::extract::State;
        use axum::http::{HeaderMap, HeaderValue};
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use bbox_corpus_core::blame_transport::{
            BLAME_TRANSPORT_VERSION, BlameExecutionPlanV1, BlamePlanTargetV1,
            OPERATOR_BLAME_REPO_ID_HEADER, OPERATOR_BLAME_ROOT_RELPATH_HEADER,
            OPERATOR_BLAME_WORKSPACE_ID_HEADER,
        };
        use bbox_corpus_core::identity::PublishedScope;
        use std::process::Command;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct FakeMcp {
            plan: BlameExecutionPlanV1,
            token: String,
            checkout_root: String,
            commit: String,
            calls: Arc<Mutex<Vec<Value>>>,
        }

        async fn fake_mcp(
            State(state): State<FakeMcp>,
            headers: HeaderMap,
            Json(request): Json<Value>,
        ) -> Response {
            let id = request["id"].clone();
            if request["method"] == "initialize" {
                assert_eq!(
                    headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok()),
                    Some(format!("Bearer {}", state.token).as_str())
                );
                assert_eq!(
                    headers
                        .get(OPERATOR_BLAME_REPO_ID_HEADER)
                        .and_then(|value| value.to_str().ok()),
                    Some("repo")
                );
                assert_eq!(
                    headers
                        .get(OPERATOR_BLAME_ROOT_RELPATH_HEADER)
                        .and_then(|value| value.to_str().ok()),
                    Some(".")
                );
                assert_eq!(
                    headers
                        .get(OPERATOR_BLAME_WORKSPACE_ID_HEADER)
                        .and_then(|value| value.to_str().ok()),
                    Some(state.plan.workspace_id.as_str())
                );
                assert!(!request.to_string().contains(&state.checkout_root));
                let mut response = Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": {"name": "fake", "version": "test"}
                    }
                }))
                .into_response();
                response
                    .headers_mut()
                    .insert("mcp-session-id", HeaderValue::from_static("operator-test"));
                return response;
            }

            let arguments = request["params"]["arguments"].clone();
            state.calls.lock().unwrap().push(arguments.clone());
            let phase = arguments["_blame_locality"]["phase"].as_str();
            let text = match phase {
                Some("plan") => serde_json::to_string(&json!({
                    "status": "blame_locality_plan",
                    "plan": state.plan,
                }))
                .unwrap(),
                Some("resolve") => {
                    assert_eq!(
                        arguments["_blame_locality"]["fact"]["attribution"]["commit_sha"],
                        state.commit
                    );
                    assert!(!arguments.to_string().contains(&state.checkout_root));
                    serde_json::to_string(&json!({
                        "status": "ok",
                        "file": "src/lib.rs",
                        "line": 1,
                    }))
                    .unwrap()
                }
                other => panic!("unexpected locality phase {other:?}"),
            };
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": text}],
                    "isError": false
                }
            }))
            .into_response()
        }

        fn git(root: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Blackbox Test")
                .env("GIT_AUTHOR_EMAIL", "blackbox@example.invalid")
                .env("GIT_COMMITTER_NAME", "Blackbox Test")
                .env("GIT_COMMITTER_EMAIL", "blackbox@example.invalid")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::create_dir_all(root.join(".bbox")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        git(&root, &["add", ".bbox/config.toml", "src/lib.rs"]);
        git(&root, &["commit", "-qm", "base"]);
        let commit = git(&root, &["rev-parse", "HEAD"]);
        let workspace_id = bbox_corpus_core::identity::ensure_checkout_id(&root).unwrap();
        let token = "c".repeat(64);
        let token_file = root.join("operator.token");
        std::fs::write(&token_file, format!("{token}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let state = FakeMcp {
            plan: BlameExecutionPlanV1 {
                version: BLAME_TRANSPORT_VERSION,
                project_id: "project".into(),
                scope: PublishedScope::try_new("repo", ".").unwrap(),
                workspace_id,
                target: BlamePlanTargetV1::WorkspacePath {
                    input_path: "src/lib.rs".into(),
                    line: 1,
                },
            },
            token,
            checkout_root: root.to_string_lossy().into_owned(),
            commit,
            calls: calls.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new()
                    .route("/mcp", post(fake_mcp))
                    .with_state(state),
            )
            .await
            .unwrap();
        });

        run(BlameArgs {
            project_root: Some(root),
            token_file,
            daemon_url: Some(format!("http://{address}")),
            file: Some(PathBuf::from("src/lib.rs")),
            entity_ref: None,
            line: Some(1),
        })
        .await
        .unwrap();
        server.abort();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["_blame_locality"]["phase"], "plan");
        assert_eq!(calls[1]["_blame_locality"]["phase"], "resolve");
    }
}
