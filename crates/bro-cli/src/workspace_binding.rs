//! `bro workspace-binding` — operator-side half of the workspace binding mint.
//!
//! The daemon route (`POST /admin/workspace-binding/mint`) is operator
//! authority: it is not an MCP tool and no agent can reach it. This verb is the
//! checkout-side companion. It resolves the checkout's committed published
//! scope and its durable workspace identity locally, asks the daemon to mint a
//! binding for that pair, and drops the resulting capability into the
//! checkout's own gitignored `.bbox/local` lane with `0600` permissions so a
//! harness or capture client running in this checkout can pick it up.
//!
//! The daemon's verification limits are documented on the route itself
//! (`src/server/workspace_binding_mint.rs`): it proves the presented path
//! against catalog attachment state, and does not re-prove the checkout on
//! disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::mcp_call::default_base_url;

/// Checkout-relative location of the binding environment file. It lives inside
/// the `.bbox/local` lane, which the checkout identity machinery keeps
/// gitignored, so a minted capability can never be committed.
const BINDING_ENV_RELPATH: &str = ".bbox/local/workspace-binding.env";
const CHECKOUT_ID_RELPATH: &str = ".bbox/local/checkout-id";

#[derive(Debug, Args)]
#[command(
    after_help = "Subcommands:\n  mint       ask the daemon to mint a workspace binding for this checkout\n  capture    run one provisional capture of this checkout's working state"
)]
pub(crate) struct WorkspaceBindingArgs {
    #[command(subcommand)]
    command: WorkspaceBindingCommand,
}

#[derive(Debug, Subcommand)]
enum WorkspaceBindingCommand {
    /// Mint a workspace binding for one local checkout and install it
    Mint(MintArgs),
    /// Capture this checkout's working state into a provisional generation
    Capture(CaptureArgs),
}

#[derive(Debug, Args)]
struct MintArgs {
    /// Checkout project root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    project_root: Option<PathBuf>,
    /// Daemon base URL. Defaults to the origin of $BLACKBOX_MCP_URL, else
    /// config [client].daemon_url, else http://127.0.0.1:<[daemon].port>.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
    /// Print the binding token to stdout once instead of writing the checkout
    /// environment file. Use when the checkout is not writable.
    #[arg(long)]
    print: bool,
}

#[derive(Debug, Args)]
struct CaptureArgs {
    /// Checkout project root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    project_root: Option<PathBuf>,
    /// Binding environment file. Defaults to the checkout's
    /// .bbox/local/workspace-binding.env, which `mint` writes.
    #[arg(long, value_name = "PATH")]
    binding_env: Option<PathBuf>,
    /// Binding capability token. Overrides the environment and the file.
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,
    /// Daemon base URL. Overrides the environment and the file.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
    /// Published scope as JSON. Overrides the environment and the file.
    #[arg(long, value_name = "JSON")]
    scope: Option<String>,
}

#[derive(Debug, Serialize)]
struct MintRequest<'a> {
    checkout_path: &'a str,
    scope: MintScope,
    workspace_id: &'a str,
}

#[derive(Debug, Serialize)]
struct MintScope {
    repo_id: String,
    bbox_root_relpath: String,
}

#[derive(Debug, Deserialize)]
struct MintResponse {
    token: String,
    project_id: String,
    workspace_id: String,
    declared_checkout_path: String,
    attachment_id: String,
    lease_id: String,
    expires_unix_secs: u64,
    provisional_capture_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct MintError {
    code: String,
    message: String,
}

pub(crate) async fn run(args: WorkspaceBindingArgs) -> anyhow::Result<()> {
    match args.command {
        WorkspaceBindingCommand::Mint(args) => mint(args).await,
        WorkspaceBindingCommand::Capture(args) => capture(args).await,
    }
}

async fn mint(args: MintArgs) -> anyhow::Result<()> {
    let requested_root = match args.project_root {
        Some(root) => root,
        None => std::env::current_dir().context("reading current directory")?,
    };
    let project_root = requested_root.canonicalize().with_context(|| {
        format!(
            "canonicalizing workspace binding project root {}",
            requested_root.display()
        )
    })?;
    if !project_root.is_dir() {
        bail!("workspace binding project root is not a directory");
    }
    let workspace_root = bbox_corpus_core::git::git_root_for_path(&project_root)
        .context("workspace binding project is not inside a Git checkout")?
        .canonicalize()
        .context("canonicalizing workspace binding Git root")?;
    if !project_root.starts_with(&workspace_root) {
        bail!("workspace binding project root is outside its Git checkout");
    }

    let scope = bbox_provenance::resolve_committed_scope(&project_root)
        .context("resolving committed workspace binding project scope")?;
    // Read, never mint: the catalog attachment is what records this checkout's
    // durable identity, so writing a fresh marker here would desynchronize the
    // capture client from the binding the daemon issues.
    let workspace_id =
        bbox_corpus_core::identity::read_checkout_id(&workspace_root.join(CHECKOUT_ID_RELPATH))
            .context("reading checkout identity marker")?
            .context(
                "checkout has no .bbox/local/checkout-id marker; attach this checkout to its \
                 project before minting a workspace binding",
            )?;

    let base_url = args.daemon_url.unwrap_or_else(default_base_url);
    let endpoint = format!(
        "{}/admin/workspace-binding/mint",
        base_url.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .post(&endpoint)
        .json(&MintRequest {
            checkout_path: &workspace_root.to_string_lossy(),
            scope: MintScope {
                repo_id: scope.repo_id().to_string(),
                bbox_root_relpath: scope.bbox_root_relpath().to_string(),
            },
            workspace_id: &workspace_id,
        })
        .send()
        .await
        .with_context(|| format!("posting workspace binding mint to {endpoint}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("reading workspace binding mint response")?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<MintError>(&body) {
            bail!(
                "daemon refused the workspace binding mint ({status}): {} - {}",
                error.code,
                error.message
            );
        }
        bail!("daemon refused the workspace binding mint ({status})");
    }
    let minted: MintResponse =
        serde_json::from_slice(&body).context("decoding workspace binding mint response")?;
    if minted.workspace_id != workspace_id {
        bail!("daemon minted a binding for a different workspace identity");
    }

    let installed = if args.print {
        println!("{}", minted.token);
        None
    } else {
        let path = workspace_root.join(BINDING_ENV_RELPATH);
        write_binding_env(&path, &minted, &base_url, &scope)
            .with_context(|| format!("writing {}", path.display()))?;
        Some(path)
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "minted",
            "project_id": minted.project_id,
            "scope": {
                "repo_id": scope.repo_id(),
                "bbox_root_relpath": scope.bbox_root_relpath(),
            },
            "workspace_id": minted.workspace_id,
            "declared_checkout_path": minted.declared_checkout_path,
            "attachment_id": minted.attachment_id,
            "lease_id": minted.lease_id,
            "expires_unix_secs": minted.expires_unix_secs,
            "provisional_capture_enabled": minted.provisional_capture_enabled,
            "binding_env_file": installed,
        }))?
    );
    Ok(())
}

/// Capture this checkout's working state into one provisional generation.
///
/// The checkout-owner half of the provisional lane. It is the same client
/// construction a managed harness performs at session start, with no agent
/// loop around it, so an operator authoring a schema can run the
/// mint, edit, capture, query loop by hand.
async fn capture(args: CaptureArgs) -> anyhow::Result<()> {
    let requested_root = match args.project_root {
        Some(root) => root,
        None => std::env::current_dir().context("reading current directory")?,
    };
    let project_root = requested_root.canonicalize().with_context(|| {
        format!(
            "canonicalizing workspace capture project root {}",
            requested_root.display()
        )
    })?;
    if !project_root.is_dir() {
        bail!("workspace capture project root is not a directory");
    }
    let workspace_root = bbox_corpus_core::git::git_root_for_path(&project_root)
        .context("workspace capture project is not inside a Git checkout")?
        .canonicalize()
        .context("canonicalizing workspace capture Git root")?;
    if !project_root.starts_with(&workspace_root) {
        bail!("workspace capture project root is outside its Git checkout");
    }

    let env_path = args
        .binding_env
        .unwrap_or_else(|| workspace_root.join(BINDING_ENV_RELPATH));
    let file_values = match std::fs::read_to_string(&env_path) {
        Ok(contents) => parse_binding_env(&contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", env_path.display()));
        }
    };
    let resolved = resolve_capture_inputs(
        CaptureOverrides {
            token: args.token,
            daemon_url: args.daemon_url,
            scope: args.scope,
        },
        &file_values,
        |name| std::env::var(name).ok(),
        &env_path,
    )?;

    let scope: bbox_corpus_core::identity::PublishedScope =
        serde_json::from_str(&resolved.scope).context("decoding the bound published scope")?;
    scope.validate()?;
    let expected_project = if scope.bbox_root_relpath() == "." {
        workspace_root.clone()
    } else {
        workspace_root.join(scope.bbox_root_relpath())
    };
    if expected_project.canonicalize().ok().as_ref() != Some(&project_root) {
        bail!("workspace capture project root does not match the bound published scope");
    }
    // Read, never mint: the identity the daemon bound is the one the capture
    // must present, so a missing marker is a refusal rather than a fresh id.
    let workspace_id =
        bbox_corpus_core::identity::read_checkout_id(&workspace_root.join(CHECKOUT_ID_RELPATH))
            .context("reading checkout identity marker")?
            .context(
                "checkout has no .bbox/local/checkout-id marker; attach this checkout to its \
                 project before capturing",
            )?;

    let client = bbox_knowledge_source_client::WorkspaceCaptureClient::new(
        &resolved.daemon_url,
        bro_protocol::WorkspaceBindingToken::parse(resolved.token)?,
        workspace_root.clone(),
        project_root.clone(),
        bro_core::WorkspaceId::parse(workspace_id.clone())?,
        scope.clone(),
    )?;
    let outcome = match client.sync_once().await {
        Ok(outcome) => outcome,
        Err(error) => {
            // A capture that failed against a daemon built differently from
            // this CLI is most likely skew, not a broken checkout: say so next
            // to the error so the operator rebuilds `bro` instead of chasing
            // the binding.
            if let Some(warning) = build_skew_warning(client.daemon_build_id().as_deref()) {
                eprintln!("warning: {warning}");
            }
            return Err(error.context("capturing the bound workspace"));
        }
    };
    if let Some(warning) = build_skew_warning(outcome.daemon_build_id.as_deref()) {
        eprintln!("warning: {warning}");
    }
    if let Some(diagnostic) = &outcome.diagnostic {
        eprintln!("warning: {diagnostic}");
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "captured",
            "source_generation_id": outcome.source_generation_id,
            "sequence": outcome.sequence,
            "reused": outcome.reused,
            "workspace_id": workspace_id,
            "project_root": project_root,
            "scope": {
                "repo_id": scope.repo_id(),
                "bbox_root_relpath": scope.bbox_root_relpath(),
            },
            "cli_build_id": CLI_BUILD_ID,
            "daemon_build_id": outcome.daemon_build_id,
            "diagnostic": outcome.diagnostic,
        }))?
    );
    Ok(())
}

/// This binary's build identity (git short sha from `build.rs`, or the
/// timestamp fallback when git was unavailable at build time).
const CLI_BUILD_ID: &str = env!("BRO_CLI_BUILD_ID");

/// A one-line skew warning when the daemon reported a build id that is not
/// this CLI's, or none at all (a daemon older than the header). Both mean the
/// two ends were built from different contracts; the response decoders are
/// tolerant, so this is advisory, but it is the first thing to check when a
/// capture misbehaves.
fn build_skew_warning(daemon_build_id: Option<&str>) -> Option<String> {
    build_skew_warning_for(CLI_BUILD_ID, daemon_build_id)
}

fn build_skew_warning_for(cli_build_id: &str, daemon_build_id: Option<&str>) -> Option<String> {
    match daemon_build_id {
        Some(daemon) if daemon == cli_build_id => None,
        Some(daemon) => Some(format!(
            "bro build {cli_build_id} is talking to daemon build {daemon}; the two were built \
             from different sources. If a capture misbehaves, rebuild and reinstall bro before \
             debugging the binding."
        )),
        None => Some(format!(
            "bro build {cli_build_id} is talking to a daemon that sent no build id (older than \
             this CLI). If a capture misbehaves, align the two builds before debugging the \
             binding."
        )),
    }
}

struct CaptureOverrides {
    token: Option<String>,
    daemon_url: Option<String>,
    scope: Option<String>,
}

#[derive(Debug)]
struct ResolvedCaptureInputs {
    token: String,
    daemon_url: String,
    scope: String,
}

/// Resolve the three binding values a capture needs. A flag wins over the
/// process environment, which wins over the installed binding file, so a
/// session already carrying a managed spawn's variables captures with no file
/// at all and an operator can point one run somewhere else without editing it.
fn resolve_capture_inputs(
    overrides: CaptureOverrides,
    file_values: &BTreeMap<String, String>,
    process_env: impl Fn(&str) -> Option<String>,
    env_path: &Path,
) -> anyhow::Result<ResolvedCaptureInputs> {
    let resolve = |flag: Option<String>, name: &str| -> anyhow::Result<String> {
        let value = flag
            .or_else(|| process_env(name).filter(|value| !value.trim().is_empty()))
            .or_else(|| file_values.get(name).cloned())
            .with_context(|| {
                format!(
                    "{name} is not set and {} does not carry it; mint a binding for this \
                     checkout first",
                    env_path.display()
                )
            })?;
        if value.trim().is_empty() {
            bail!("{name} is empty");
        }
        Ok(value)
    };
    Ok(ResolvedCaptureInputs {
        token: resolve(overrides.token, bro_protocol::WORKSPACE_BINDING_ENV)?,
        daemon_url: resolve(overrides.daemon_url, bro_protocol::KNOWLEDGE_SOURCE_URL_ENV)?,
        scope: resolve(overrides.scope, bro_protocol::WORKSPACE_SCOPE_ENV)?,
    })
}

/// Read a binding environment file the way a shell would: `KEY=value` lines,
/// blanks and `#` comments skipped, and one layer of surrounding single or
/// double quotes removed.
fn parse_binding_env(contents: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        values.insert(key.trim().to_string(), unquote(value).to_string());
    }
    values
}

fn unquote(value: &str) -> &str {
    for quote in ['\'', '"'] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Render the three environment variables a bound harness or capture client
/// reads, using the same names the managed spawn path exports.
///
/// The scope is single-quoted. It is JSON, and the documented way to load this
/// file is to source it, so an unquoted value would reach the reader with its
/// double quotes stripped by the shell and fail to parse. JSON never contains
/// a single quote, so the quoting is unambiguous, and it reads correctly both
/// through a shell and through an EnvironmentFile-style reader.
fn binding_env_contents(
    minted: &MintResponse,
    base_url: &str,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> anyhow::Result<String> {
    Ok(format!(
        "{}={}\n{}={}\n{}='{}'\n",
        bro_protocol::WORKSPACE_BINDING_ENV,
        minted.token,
        bro_protocol::KNOWLEDGE_SOURCE_URL_ENV,
        base_url.trim_end_matches('/'),
        bro_protocol::WORKSPACE_SCOPE_ENV,
        serde_json::to_string(scope)?,
    ))
}

#[cfg(unix)]
fn write_binding_env(
    path: &Path,
    minted: &MintResponse,
    base_url: &str,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let contents = binding_env_contents(minted, base_url, scope)?;
    // Truncate through a fresh 0600 handle so a pre-existing world-readable
    // file cannot keep its old mode while gaining a new secret.
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_binding_env(
    path: &Path,
    minted: &MintResponse,
    base_url: &str,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> anyhow::Result<()> {
    std::fs::write(path, binding_env_contents(minted, base_url, scope)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_skew_warning_names_both_builds_and_is_silent_when_equal() {
        assert_eq!(build_skew_warning_for("abc123", Some("abc123")), None);
        let skew = build_skew_warning_for("abc123", Some("def456")).unwrap();
        assert!(skew.contains("abc123") && skew.contains("def456"), "{skew}");
        let missing = build_skew_warning_for("abc123", None).unwrap();
        assert!(missing.contains("no build id"), "{missing}");
    }

    fn minted() -> MintResponse {
        MintResponse {
            token: "a".repeat(64),
            project_id: "p_00000000000000000000000000000001".into(),
            workspace_id: "0123456789abcdef0123456789abcdef".into(),
            declared_checkout_path: "/checkout".into(),
            attachment_id: "att_11111111111111111111111111111111".into(),
            lease_id: "operator-workspace-binding:0123456789abcdef0123456789abcdef".into(),
            expires_unix_secs: 42,
            provisional_capture_enabled: true,
        }
    }

    #[test]
    fn binding_env_uses_the_managed_spawn_variable_names() {
        let scope = bbox_corpus_core::identity::PublishedScope::try_new("repo", ".").unwrap();
        let rendered = binding_env_contents(&minted(), "http://127.0.0.1:7299/", &scope).unwrap();
        assert!(rendered.contains(&format!(
            "{}={}",
            bro_protocol::WORKSPACE_BINDING_ENV,
            "a".repeat(64)
        )));
        assert!(rendered.contains(&format!(
            "{}=http://127.0.0.1:7299\n",
            bro_protocol::KNOWLEDGE_SOURCE_URL_ENV
        )));
        assert!(rendered.contains(&format!(
            "{}='{{\"repo_id\":\"repo\",\"bbox_root_relpath\":\".\"}}'",
            bro_protocol::WORKSPACE_SCOPE_ENV
        )));
    }

    /// The runbook loads this file by sourcing it. An unquoted JSON value
    /// reaches the reader with its double quotes stripped, so the scope must
    /// survive a real shell round-trip and still parse.
    #[cfg(unix)]
    #[test]
    fn binding_env_scope_survives_being_sourced_by_a_shell() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("workspace-binding.env");
        let scope =
            bbox_corpus_core::identity::PublishedScope::try_new("repo", "services/api").unwrap();
        write_binding_env(&path, &minted(), "http://127.0.0.1:7299", &scope).unwrap();

        let script = format!(
            "set -a; . '{}'; set +a; printf '%s' \"${}\"",
            path.display(),
            bro_protocol::WORKSPACE_SCOPE_ENV
        );
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let sourced = String::from_utf8(output.stdout).unwrap();

        let parsed: bbox_corpus_core::identity::PublishedScope =
            serde_json::from_str(&sourced).expect("sourced scope must still be JSON");
        assert_eq!(parsed, scope);
    }

    #[test]
    fn binding_env_parses_quoted_and_unquoted_values() {
        let contents = format!(
            "# a comment\n\n{}=abc\n{}=http://127.0.0.1:7299\n{}='{{\"repo_id\":\"repo\",\"bbox_root_relpath\":\".\"}}'\n",
            bro_protocol::WORKSPACE_BINDING_ENV,
            bro_protocol::KNOWLEDGE_SOURCE_URL_ENV,
            bro_protocol::WORKSPACE_SCOPE_ENV,
        );
        let values = parse_binding_env(&contents);
        assert_eq!(
            values.get(bro_protocol::WORKSPACE_BINDING_ENV).unwrap(),
            "abc"
        );
        assert_eq!(
            values.get(bro_protocol::KNOWLEDGE_SOURCE_URL_ENV).unwrap(),
            "http://127.0.0.1:7299"
        );
        assert_eq!(
            values.get(bro_protocol::WORKSPACE_SCOPE_ENV).unwrap(),
            "{\"repo_id\":\"repo\",\"bbox_root_relpath\":\".\"}"
        );
    }

    #[test]
    fn capture_inputs_prefer_flags_then_environment_then_file() {
        let file_values = BTreeMap::from([
            (
                bro_protocol::WORKSPACE_BINDING_ENV.to_string(),
                "file-token".to_string(),
            ),
            (
                bro_protocol::KNOWLEDGE_SOURCE_URL_ENV.to_string(),
                "http://file".to_string(),
            ),
            (
                bro_protocol::WORKSPACE_SCOPE_ENV.to_string(),
                "{\"repo_id\":\"repo\",\"bbox_root_relpath\":\".\"}".to_string(),
            ),
        ]);
        let resolved = resolve_capture_inputs(
            CaptureOverrides {
                token: Some("flag-token".to_string()),
                daemon_url: None,
                scope: None,
            },
            &file_values,
            |name| {
                (name == bro_protocol::KNOWLEDGE_SOURCE_URL_ENV).then(|| "http://env".to_string())
            },
            Path::new("/checkout/.bbox/local/workspace-binding.env"),
        )
        .unwrap();

        assert_eq!(resolved.token, "flag-token");
        assert_eq!(resolved.daemon_url, "http://env");
        assert_eq!(
            resolved.scope,
            "{\"repo_id\":\"repo\",\"bbox_root_relpath\":\".\"}"
        );
    }

    #[test]
    fn capture_inputs_name_the_missing_variable_and_the_file() {
        let error = resolve_capture_inputs(
            CaptureOverrides {
                token: None,
                daemon_url: None,
                scope: None,
            },
            &BTreeMap::new(),
            |_| None,
            Path::new("/checkout/.bbox/local/workspace-binding.env"),
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.contains(bro_protocol::WORKSPACE_BINDING_ENV),
            "{error}"
        );
        assert!(error.contains("workspace-binding.env"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn binding_env_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("workspace-binding.env");
        let scope = bbox_corpus_core::identity::PublishedScope::try_new("repo", ".").unwrap();
        std::fs::write(&path, "stale world readable").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_binding_env(&path, &minted(), "http://127.0.0.1:7299", &scope).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(&"a".repeat(64))
        );
    }
}
