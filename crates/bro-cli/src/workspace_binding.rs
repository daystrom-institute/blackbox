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
    after_help = "Subcommands:\n  mint    ask the daemon to mint a workspace binding for this checkout"
)]
pub(crate) struct WorkspaceBindingArgs {
    #[command(subcommand)]
    command: WorkspaceBindingCommand,
}

#[derive(Debug, Subcommand)]
enum WorkspaceBindingCommand {
    /// Mint a workspace binding for one local checkout and install it
    Mint(MintArgs),
}

#[derive(Debug, Args)]
struct MintArgs {
    /// Checkout project root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    project_root: Option<PathBuf>,
    /// Daemon base URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
    /// Print the binding token to stdout once instead of writing the checkout
    /// environment file. Use when the checkout is not writable.
    #[arg(long)]
    print: bool,
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

/// Render the three environment variables a bound harness or capture client
/// reads, using the same names the managed spawn path exports.
fn binding_env_contents(
    minted: &MintResponse,
    base_url: &str,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> anyhow::Result<String> {
    Ok(format!(
        "{}={}\n{}={}\n{}={}\n",
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
            "{}={{\"repo_id\":\"repo\",\"bbox_root_relpath\":\".\"}}",
            bro_protocol::WORKSPACE_SCOPE_ENV
        )));
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
